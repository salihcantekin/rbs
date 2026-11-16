//! The text worker (ADR-0129 decisions 6 and 7, spec `scene-sources.md`, "Text
//! worker") with the fake renderer and a scripted file probe. Waits are for
//! conditions; real time bounds them only as hang guards (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rbs_core::capability::CapabilitySet;
use rbs_core::time::{TestClock, Timestamp};
use rbs_graph::runtime::param::{ParamReceiver, channel};
use rbs_nodes::TextFrame;
use rbs_scene::model::{SettingValue, Settings, SourceId};
use rbs_sources::text::{TextSettings, validate_text};
use rbs_sources::text_file::{FileMeta, FileProbe};
use rbs_sources::text_worker::{
    Families, MAX_DETACHED_PROBES, ProblemSlot, TextClock, TextProblem, TextWorker,
};
use rbs_sources::types::keys;
use rbs_text::{RenderNotes, TextBlock, TextError, TextPicture, TextRenderer};
use rbs_text_test::FakeTextRenderer;

/// Hang guard for conditions that hold within milliseconds.
const GUARD: Duration = Duration::from_secs(10);

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < GUARD, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

/// The fake renderer, counting renders and trims; it panics on the text "PANIC".
#[derive(Default)]
struct Counting {
    inner: FakeTextRenderer,
    renders: AtomicUsize,
    trims: AtomicUsize,
}

impl TextRenderer for Counting {
    fn id(&self) -> &str {
        "counting"
    }
    fn capabilities(&self) -> &CapabilitySet {
        self.inner.capabilities()
    }
    fn render(&self, block: &TextBlock) -> Result<(TextPicture, RenderNotes), TextError> {
        self.renders.fetch_add(1, Ordering::SeqCst);
        assert!(block.text != "PANIC", "renderer panics on purpose");
        self.inner.render(block)
    }
    fn families(&self) -> Result<Vec<String>, TextError> {
        self.inner.families()
    }
    fn measure(&self, block: &TextBlock, max_width: Option<u32>) -> Result<[u32; 2], TextError> {
        self.inner.measure(block, max_width)
    }
    fn trim(&self) {
        self.trims.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct File {
    bytes: Vec<u8>,
    modified: SystemTime,
}

/// Files in memory. A path in `stuck` blocks its read until `release`.
#[derive(Default)]
struct Files {
    files: Mutex<BTreeMap<PathBuf, File>>,
    stuck: Mutex<BTreeSet<PathBuf>>,
    released: Mutex<bool>,
    wake: Condvar,
    reads: AtomicUsize,
    looks: AtomicUsize,
}

impl Files {
    fn put(&self, path: &str, text: &str, modified: SystemTime) {
        self.files.lock().unwrap().insert(
            PathBuf::from(path),
            File {
                bytes: text.as_bytes().to_vec(),
                modified,
            },
        );
    }
    fn stick(&self, path: &str) {
        self.stuck.lock().unwrap().insert(PathBuf::from(path));
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

impl FileProbe for Files {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.looks.fetch_add(1, Ordering::SeqCst);
        if self.stuck.lock().unwrap().contains(path) {
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
        }
        if self.files.lock().unwrap().contains_key(path) {
            Ok(path.to_owned())
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }
    fn metadata(&self, path: &Path) -> io::Result<Option<FileMeta>> {
        let files = self.files.lock().unwrap();
        let f = files.get(path).ok_or(io::ErrorKind::NotFound)?;
        Ok(Some(FileMeta {
            len: f.bytes.len() as u64,
            modified: Some(f.modified),
        }))
    }
    fn read(&self, path: &Path, head: u64, tail: u64) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let files = self.files.lock().unwrap();
        let f = files.get(path).ok_or(io::ErrorKind::NotFound)?;
        let b = &f.bytes;
        let h = b[..b.len().min(head as usize)].to_vec();
        let t = b[b.len().saturating_sub(tail as usize)..].to_vec();
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok((h, t))
    }
}

struct Wall(Mutex<SystemTime>);

impl TextClock for Wall {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

struct Rig {
    worker: TextWorker,
    renderer: Arc<Counting>,
    files: Arc<Files>,
    clock: TestClock,
    wall: Arc<Wall>,
}

/// Wall time long after the files were written (outside the recent window).
const T0: u64 = 1_800_000_000;

fn rig() -> Rig {
    let renderer = Arc::new(Counting::default());
    let files = Arc::new(Files::default());
    let clock = TestClock::new();
    let wall = Arc::new(Wall(Mutex::new(
        SystemTime::UNIX_EPOCH + Duration::from_secs(T0),
    )));
    let worker = TextWorker::new(
        vec![Arc::clone(&renderer) as Arc<dyn TextRenderer>],
        Arc::clone(&files) as Arc<dyn FileProbe>,
        Arc::new(clock.clone()),
        Arc::clone(&wall) as Arc<dyn TextClock>,
    );
    Rig {
        worker,
        renderer,
        files,
        clock,
        wall,
    }
}

fn old() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(T0 - 3600)
}

fn settings(text: &str, file: Option<&str>) -> TextSettings {
    let mut s = Settings::new();
    s.insert(keys::WIDTH.into(), SettingValue::Int(120));
    s.insert(keys::HEIGHT.into(), SettingValue::Int(40));
    s.insert(keys::SIZE.into(), SettingValue::Float(20.0));
    s.insert(keys::TEXT.into(), SettingValue::Text(text.into()));
    if let Some(f) = file {
        s.insert(keys::FILE.into(), SettingValue::Text(f.into()));
    }
    validate_text(&s).unwrap()
}

/// The picture the fake draws for `text` with [`settings`].
fn expected(text: &str) -> Vec<u8> {
    let mut block = settings("", None).block;
    block.text = text.into();
    FakeTextRenderer::new()
        .render(&block)
        .unwrap()
        .0
        .rgba()
        .to_vec()
}

fn next_frame(rx: &mut ParamReceiver<Option<TextFrame>>, after: u64) -> TextFrame {
    wait_for("a picture", || {
        rx.update();
        rx.get().as_ref().is_some_and(|f| f.request > after)
    });
    rx.get().clone().unwrap()
}

fn shows(rx: &mut ParamReceiver<Option<TextFrame>>, text: &str) {
    let want = expected(text);
    wait_for(&format!("the picture of {text:?}"), || {
        rx.update();
        rx.get()
            .as_ref()
            .is_some_and(|f| f.picture.rgba() == want.as_slice())
    });
}

fn src(n: u32) -> SourceId {
    SourceId::from_raw(n)
}

/// Advances the master clock by one poll interval and ticks.
fn poll(r: &mut Rig) {
    r.clock.advance(Duration::from_millis(500));
    r.worker.tick();
}

fn problems(r: &mut Rig) -> Vec<(SourceId, ProblemSlot, Option<TextProblem>)> {
    r.worker.take_problems()
}

#[test]
fn nothing_runs_until_a_source_is_attached() {
    let mut r = rig();
    r.worker.tick();
    assert!(!r.worker.is_running());
    r.worker.set_on_air(BTreeSet::from([src(1)]));
    r.worker.tick();
    assert!(!r.worker.is_running());
}

#[test]
fn an_attached_source_gets_its_picture_and_edits_redraw_it() {
    let mut r = rig();
    let (tx, mut rx) = channel(None);
    let request = r.worker.attach(src(1), tx, settings("Hello", None));
    assert!(r.worker.is_running());
    let first = next_frame(&mut rx, 0);
    assert_eq!(first.request, request);
    assert_eq!((first.picture.width(), first.picture.height()), (120, 40));
    shows(&mut rx, "Hello");

    // The same settings again: nothing to do.
    assert_eq!(r.worker.set_settings(src(1), settings("Hello", None)), None);
    let request = r
        .worker
        .set_settings(src(1), settings("Bye", None))
        .unwrap();
    let edited = next_frame(&mut rx, first.request);
    assert_eq!(edited.request, request);
    assert_ne!(edited.settings_generation, first.settings_generation);
    shows(&mut rx, "Bye");
}

#[test]
fn a_recreated_node_gets_the_cached_picture_without_a_render() {
    let mut r = rig();
    let (tx, mut rx) = channel(None);
    r.worker.attach(src(1), tx, settings("Cached", None));
    let first = next_frame(&mut rx, 0);
    let renders = r.renderer.renders.load(Ordering::SeqCst);
    r.worker.detach(src(1));
    let (tx, mut rx2) = channel(None);
    r.worker.attach(src(1), tx, settings("Cached", None));
    wait_for("the cached picture", || {
        rx2.update();
        rx2.get().is_some()
    });
    let again = rx2.get().clone().unwrap();
    assert!(Arc::ptr_eq(&again.picture, &first.picture));
    assert_eq!(r.renderer.renders.load(Ordering::SeqCst), renders);
    drop(rx);
}

#[test]
fn a_file_is_shown_and_a_change_needs_two_agreeing_polls() {
    let mut r = rig();
    r.files.put("/f/a.txt", "one\ntwo\nthree\n", old());
    let mut s = settings("", Some("/f/a.txt"));
    s.file_lines = 2;
    r.worker.set_on_air(BTreeSet::from([src(1)]));
    let (tx, mut rx) = channel(None);
    r.worker.attach(src(1), tx, s);
    shows(&mut rx, "two\nthree");

    // Rewritten just now: the first poll only remembers it.
    let now = r.wall.now();
    r.files.put("/f/a.txt", "one\ntwo\nfour\n", now);
    let reads = r.files.reads.load(Ordering::SeqCst);
    poll(&mut r);
    wait_for("a read", || r.files.reads.load(Ordering::SeqCst) > reads);
    let reads = r.files.reads.load(Ordering::SeqCst);
    poll(&mut r);
    wait_for("a second read", || {
        r.files.reads.load(Ordering::SeqCst) > reads
    });
    shows(&mut rx, "two\nfour");
}

#[test]
fn an_unchanged_old_file_is_not_read_again() {
    let mut r = rig();
    r.files.put("/f/a.txt", "same", old());
    r.worker.set_on_air(BTreeSet::from([src(1)]));
    let (tx, mut rx) = channel(None);
    r.worker.attach(src(1), tx, settings("", Some("/f/a.txt")));
    shows(&mut rx, "same");
    let reads = r.files.reads.load(Ordering::SeqCst);
    for _ in 0..3 {
        let looks = r.files.looks.load(Ordering::SeqCst);
        poll(&mut r);
        wait_for("a poll", || r.files.looks.load(Ordering::SeqCst) > looks);
    }
    assert_eq!(r.files.reads.load(Ordering::SeqCst), reads);
}

#[test]
fn a_file_off_air_is_not_polled() {
    let mut r = rig();
    r.files.put("/f/a.txt", "x", old());
    let (tx, _rx) = channel(None);
    r.worker.attach(src(1), tx, settings("", Some("/f/a.txt")));
    for _ in 0..3 {
        poll(&mut r);
    }
    // Ask something of the worker that answers in order, then check nothing polled.
    assert_eq!(r.files.looks.load(Ordering::SeqCst), 0);
}

#[test]
fn a_missing_file_shows_nothing_until_it_appears() {
    let mut r = rig();
    r.worker.set_on_air(BTreeSet::from([src(1)]));
    let (tx, mut rx) = channel(None);
    r.worker
        .attach(src(1), tx, settings("", Some("/f/later.txt")));
    shows(&mut rx, "");
    let mut seen = Vec::new();
    wait_for("the missing file problem", || {
        seen.extend(problems(&mut r));
        seen.contains(&(src(1), ProblemSlot::File, Some(TextProblem::FileNotFound)))
    });
    assert_eq!(TextProblem::FileNotFound.message(), "File not found");

    r.files.put("/f/later.txt", "Here", old());
    poll(&mut r);
    shows(&mut rx, "Here");
    wait_for("the problem to clear", || {
        problems(&mut r).contains(&(src(1), ProblemSlot::File, None))
    });
}

#[test]
fn bytes_that_are_not_text_keep_the_last_picture() {
    let mut r = rig();
    r.files.put("/f/a.txt", "fine", old());
    r.worker.set_on_air(BTreeSet::from([src(1)]));
    let (tx, mut rx) = channel(None);
    r.worker.attach(src(1), tx, settings("", Some("/f/a.txt")));
    shows(&mut rx, "fine");
    r.files.files.lock().unwrap().insert(
        PathBuf::from("/f/a.txt"),
        File {
            bytes: vec![0xC3, 0x28],
            modified: old() + Duration::from_secs(1),
        },
    );
    let mut seen = Vec::new();
    for _ in 0..2 {
        let reads = r.files.reads.load(Ordering::SeqCst);
        poll(&mut r);
        wait_for("a read", || r.files.reads.load(Ordering::SeqCst) > reads);
    }
    wait_for("the not-text problem", || {
        seen.extend(problems(&mut r));
        seen.contains(&(src(1), ProblemSlot::File, Some(TextProblem::NotText)))
    });
    rx.update();
    assert_eq!(rx.get().as_ref().unwrap().picture.rgba(), expected("fine"));
}

#[test]
fn a_hung_file_call_only_stops_its_own_source() {
    let mut r = rig();
    r.files.put("/f/hang.txt", "h", old());
    r.files.put("/f/ok.txt", "ok", old());
    r.files.stick("/f/hang.txt");
    r.worker.set_on_air(BTreeSet::from([src(1), src(2)]));
    let (tx1, _rx1) = channel(None);
    let (tx2, mut rx2) = channel(None);
    r.worker
        .attach(src(1), tx1, settings("", Some("/f/hang.txt")));
    wait_for("the stuck call", || {
        r.files.looks.load(Ordering::SeqCst) >= 1
    });
    r.clock.advance(Duration::from_secs(6));
    r.worker.tick();
    let got = problems(&mut r);
    assert!(
        got.contains(&(
            src(1),
            ProblemSlot::File,
            Some(TextProblem::FileNotAnswering)
        )),
        "{got:?}"
    );
    assert_eq!(r.worker.detached_probes(), 1);

    // The other source is served by a new probe and has no problem.
    r.worker
        .attach(src(2), tx2, settings("", Some("/f/ok.txt")));
    shows(&mut rx2, "ok");
    r.files
        .put("/f/ok.txt", "ok 2", old() + Duration::from_secs(5));
    poll(&mut r);
    poll(&mut r);
    shows(&mut rx2, "ok 2");
    assert!(
        problems(&mut r)
            .iter()
            .all(|p| p.0 != src(2) || p.2.is_none())
    );

    // When the call returns, the path is polled again and the problem clears.
    r.files.release();
    wait_for("the detached probe to exit", || {
        r.worker.tick();
        r.worker.detached_probes() == 0
    });
    let got = problems(&mut r);
    assert!(got.contains(&(src(1), ProblemSlot::File, None)), "{got:?}");
}

#[test]
fn stuck_probes_never_pass_the_cap_and_stop_does_not_block() {
    let mut r = rig();
    for n in 1..=4 {
        let path = format!("/f/stuck{n}.txt");
        r.files.put(&path, "x", old());
        r.files.stick(&path);
    }
    r.worker
        .set_on_air((1..=4).map(src).collect::<BTreeSet<_>>());
    let mut keep = Vec::new();
    for n in 1..=4u32 {
        let (tx, rx) = channel(None);
        keep.push(rx);
        let looks = r.files.looks.load(Ordering::SeqCst);
        r.worker
            .attach(src(n), tx, settings("", Some(&format!("/f/stuck{n}.txt"))));
        if n <= 3 {
            wait_for("a stuck call", || {
                r.files.looks.load(Ordering::SeqCst) > looks
            });
        }
        r.clock.advance(Duration::from_secs(6));
        r.worker.tick();
        assert!(r.worker.detached_probes() <= MAX_DETACHED_PROBES);
    }
    assert_eq!(r.worker.detached_probes(), MAX_DETACHED_PROBES);

    // Stop with a stuck probe returns at once; a restart stays under the cap.
    let start = Instant::now();
    for n in 1..=4 {
        r.worker.detach(src(n));
    }
    r.worker.stop();
    assert!(start.elapsed() < GUARD);
    assert!(!r.worker.is_running());
    let (tx, _rx) = channel(None);
    r.worker.attach(src(9), tx, settings("again", None));
    r.clock.advance(Duration::from_secs(6));
    r.worker.tick();
    assert!(r.worker.detached_probes() <= MAX_DETACHED_PROBES);
    r.files.release();
}

#[test]
fn the_threads_stop_when_idle_and_trim_the_renderer() {
    let mut r = rig();
    let (tx, mut rx) = channel(None);
    r.worker.attach(src(1), tx, settings("x", None));
    next_frame(&mut rx, 0);
    r.worker.detach(src(1));
    r.clock.advance(Duration::from_secs(29));
    r.worker.tick();
    assert!(r.worker.is_running());
    r.clock.advance(Duration::from_secs(1));
    r.worker.tick();
    assert!(!r.worker.is_running());
    assert_eq!(r.renderer.trims.load(Ordering::SeqCst), 1);
}

#[test]
fn a_renderer_panic_is_a_problem_and_the_worker_goes_on() {
    let mut r = rig();
    let (tx, mut rx) = channel(None);
    r.worker.attach(src(1), tx, settings("PANIC", None));
    let mut seen = Vec::new();
    wait_for("the render problem", || {
        seen.extend(problems(&mut r));
        seen.iter()
            .any(|p| p.0 == src(1) && matches!(p.2, Some(TextProblem::RenderFailed(_))))
    });
    r.worker.set_settings(src(1), settings("fine", None));
    shows(&mut rx, "fine");
    assert_eq!(
        r.renderer.trims.load(Ordering::SeqCst),
        1,
        "trimmed after the panic"
    );
}

#[test]
fn the_font_list_is_cached() {
    let mut r = rig();
    assert_eq!(r.worker.families(), Families::NotReady);
    wait_for("the font list", || {
        matches!(r.worker.families(), Families::Ready(_))
    });
    let Families::Ready(list) = r.worker.families() else {
        unreachable!()
    };
    assert_eq!(
        list.as_slice(),
        rbs_text_test::FAMILIES.map(String::from).as_slice()
    );
}

#[test]
fn problems_name_no_path() {
    for p in [
        TextProblem::FileNotFound,
        TextProblem::NotText,
        TextProblem::FileNotAnswering,
        TextProblem::CouldNotDraw,
        TextProblem::FontMissing,
    ] {
        assert!(!p.message().contains('/'));
    }
    assert!(TextProblem::FontMissing.fault().is_none());
    let _ = Timestamp::ZERO;
    let _ = AtomicBool::new(false);
}
