//! The text worker (ADR-0129 decision 7, spec `scene-sources.md`, "Text worker"): one
//! render thread that draws every text source in order and owns each node's picture
//! sender, and one probe thread for file access. Driven from the control thread; no
//! call here blocks it.
//!
//! Installed state costs nothing: no thread exists until the first source is attached
//! or the font list is asked for, and both threads stop when no text source is left
//! and nothing was asked for [`IDLE_STOP_AFTER`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use rbs_core::fault::SourceFault;
use rbs_core::time::{Clock, Timestamp};
use rbs_graph::runtime::param::ParamSender;
use rbs_nodes::TextFrame;
use rbs_scene::model::SourceId;
use rbs_text::{TextBlock, TextRenderer};

use crate::text::{TextSettings, pick_renderer};
use crate::text_file::{
    FileMeta, FileProbe, FileTextError, MAX_FILE_BYTES, content_hash, decode, last_lines,
};

/// How often on-air files are polled, by the injected master clock.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// A render or file call longer than this is reported, and never waited on again.
pub const HANG_AFTER: Duration = Duration::from_secs(5);
/// The threads stop this long after the last text source left and the last request.
pub const IDLE_STOP_AFTER: Duration = Duration::from_secs(30);
/// Most probe threads left behind inside a call that does not return.
pub const MAX_DETACHED_PROBES: usize = 2;
/// A file modified this recently is read and hashed on every poll, so a same-size
/// rewrite on a coarse file system is seen.
pub const RECENT_WINDOW: Duration = Duration::from_secs(2);
/// The font list is refreshed at most this often.
pub const FAMILIES_REFRESH: Duration = Duration::from_secs(60);

/// Local wall time, injected so tests control "now" for the recent-file window (and,
/// from phase (d), clocks).
pub trait TextClock: Send + Sync {
    /// The current wall time.
    fn now(&self) -> SystemTime;
}

/// The system's wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemTextClock;

impl TextClock for SystemTextClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A problem of one text source. Messages never contain a path or file content.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextProblem {
    /// The file does not exist or is not a regular local file.
    FileNotFound,
    /// The file is not UTF-8 or UTF-16 text.
    NotText,
    /// A file call has not returned for [`HANG_AFTER`].
    FileNotAnswering,
    /// The renderer failed or panicked; its message.
    RenderFailed(String),
    /// A render has not returned for [`HANG_AFTER`].
    CouldNotDraw,
    /// The font family is not installed; the system font was used (a note).
    FontMissing,
}

impl TextProblem {
    /// The sentence shown to the user.
    pub fn message(&self) -> String {
        match self {
            TextProblem::FileNotFound => "File not found".to_owned(),
            TextProblem::NotText => "The file is not readable text".to_owned(),
            TextProblem::FileNotAnswering => "The file does not answer".to_owned(),
            TextProblem::RenderFailed(m) => m.clone(),
            TextProblem::CouldNotDraw => "Text could not be drawn".to_owned(),
            TextProblem::FontMissing => {
                "The font is not installed; the system font is used".to_owned()
            }
        }
    }

    /// The source fault it maps to (ADR-0077); `None` for a note.
    pub fn fault(&self) -> Option<SourceFault> {
        match self {
            TextProblem::FileNotFound => Some(SourceFault::TargetGone),
            TextProblem::FontMissing => None,
            _ => Some(SourceFault::Other),
        }
    }
}

/// Which part of the worker a problem comes from; each slot is set and cleared on its
/// own, so a font note does not clear a missing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProblemSlot {
    /// File access.
    File,
    /// Drawing.
    Render,
}

/// A problem set (`Some`) or cleared (`None`) for a source.
pub type ProblemUpdate = (SourceId, ProblemSlot, Option<TextProblem>);

/// The answer to a font list request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Families {
    /// The cached list.
    Ready(Arc<Vec<String>>),
    /// The worker is building the first list; ask again later.
    NotReady,
}

/// State both threads and the control side share.
struct Shared {
    clock: Arc<dyn Clock>,
    problems: Mutex<Vec<ProblemUpdate>>,
    /// Master-clock nanoseconds + 1 when the current render started; 0 when idle.
    render_busy: AtomicU64,
    /// The source being drawn (raw id + 1; 0 none).
    render_source: AtomicU64,
    families: Mutex<Option<(Arc<Vec<String>>, Timestamp)>>,
    families_pending: AtomicBool,
    next_request: AtomicU64,
}

impl Shared {
    fn post(&self, update: ProblemUpdate) {
        self.problems
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(update);
    }

    fn request(&self) -> u64 {
        self.next_request.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn stamp(&self) -> u64 {
        self.clock.now().as_nanos().saturating_add(1)
    }
}

enum RenderCmd {
    Attach {
        source: SourceId,
        frames: ParamSender<Option<TextFrame>>,
        settings: Arc<TextSettings>,
        generation: u64,
        request: u64,
    },
    Settings {
        source: SourceId,
        settings: Arc<TextSettings>,
        generation: u64,
        request: u64,
    },
    File {
        source: SourceId,
        generation: u64,
        content: String,
        request: u64,
    },
    Rerender {
        source: SourceId,
        request: u64,
    },
    Detach {
        source: SourceId,
    },
    Families,
    Stop,
}

enum ProbeCmd {
    Poll {
        source: SourceId,
        path: String,
        file_lines: u32,
        generation: u64,
    },
    Forget {
        source: SourceId,
    },
    Stop,
}

/// A probe thread's busy marker, shared with the control side.
#[derive(Default)]
struct ProbeBusy {
    /// Master-clock nanoseconds + 1 when the current call started; 0 when idle.
    since: AtomicU64,
    /// The source the call is for (raw id + 1).
    source: AtomicU64,
    exited: AtomicBool,
}

struct ProbeThread {
    tx: Sender<ProbeCmd>,
    busy: Arc<ProbeBusy>,
    handle: Option<JoinHandle<()>>,
}

impl ProbeThread {
    fn is_busy(&self) -> bool {
        self.busy.since.load(Ordering::Acquire) != 0
    }
}

struct RenderThread {
    tx: Sender<RenderCmd>,
    handle: Option<JoinHandle<()>>,
}

/// What the control side knows about a source.
struct Known {
    settings: Arc<TextSettings>,
    generation: u64,
    attached: bool,
}

/// The text worker's control-side handle. Control thread only.
pub struct TextWorker {
    renderers: Vec<Arc<dyn TextRenderer>>,
    files: Arc<dyn FileProbe>,
    wall: Arc<dyn TextClock>,
    shared: Arc<Shared>,
    render: Option<RenderThread>,
    probe: Option<ProbeThread>,
    detached: Vec<ProbeThread>,
    known: BTreeMap<SourceId, Known>,
    next_generation: u64,
    on_air: BTreeSet<SourceId>,
    /// Sources whose file call hung, with the probe that holds it.
    hung: BTreeMap<SourceId, Arc<ProbeBusy>>,
    /// The render that hung was reported.
    render_hang_reported: bool,
    last_poll: Option<Timestamp>,
    last_use: Timestamp,
}

impl TextWorker {
    /// A worker that draws with `renderers` (first match wins, ADR-0129 decision 2),
    /// reads files through `files` and takes time from the master `clock` (polling,
    /// hangs, idle stop) and `wall` (the recent-file window). Starts no thread.
    pub fn new(
        renderers: Vec<Arc<dyn TextRenderer>>,
        files: Arc<dyn FileProbe>,
        clock: Arc<dyn Clock>,
        wall: Arc<dyn TextClock>,
    ) -> Self {
        Self {
            renderers,
            files,
            wall,
            shared: Arc::new(Shared {
                clock,
                problems: Mutex::new(Vec::new()),
                render_busy: AtomicU64::new(0),
                render_source: AtomicU64::new(0),
                families: Mutex::new(None),
                families_pending: AtomicBool::new(false),
                next_request: AtomicU64::new(0),
            }),
            render: None,
            probe: None,
            detached: Vec::new(),
            known: BTreeMap::new(),
            next_generation: 0,
            on_air: BTreeSet::new(),
            hung: BTreeMap::new(),
            render_hang_reported: false,
            last_poll: None,
            last_use: Timestamp::ZERO,
        }
    }

    /// Whether the worker's threads run.
    pub fn is_running(&self) -> bool {
        self.render.is_some()
    }

    /// Probe threads left inside a call that has not returned.
    pub fn detached_probes(&self) -> usize {
        self.detached
            .iter()
            .filter(|p| !p.busy.exited.load(Ordering::Acquire))
            .count()
    }

    fn now(&self) -> Timestamp {
        self.shared.clock.now()
    }

    fn ensure_running(&mut self) {
        self.last_use = self.now();
        if self.render.is_none() {
            let (tx, rx) = channel();
            let shared = Arc::clone(&self.shared);
            let renderers = self.renderers.clone();
            let handle = std::thread::Builder::new()
                .name("rbs-text-render".into())
                .spawn(move || render_loop(&rx, &shared, &renderers))
                .ok();
            self.render = Some(RenderThread { tx, handle });
        }
        if self.probe.is_none() {
            self.probe = self.spawn_probe();
        }
    }

    fn spawn_probe(&self) -> Option<ProbeThread> {
        let render_tx = self.render.as_ref()?.tx.clone();
        let (tx, rx) = channel();
        let busy = Arc::new(ProbeBusy::default());
        let ctx = ProbeCtx {
            files: Arc::clone(&self.files),
            wall: Arc::clone(&self.wall),
            shared: Arc::clone(&self.shared),
            busy: Arc::clone(&busy),
            render: render_tx,
        };
        let handle = std::thread::Builder::new()
            .name("rbs-text-probe".into())
            .spawn(move || probe_loop(&rx, &ctx))
            .ok();
        Some(ProbeThread { tx, busy, handle })
    }

    fn send(&self, cmd: RenderCmd) {
        if let Some(r) = &self.render {
            let _ = r.tx.send(cmd);
        }
    }

    fn generation_for(&mut self, source: SourceId, settings: &TextSettings) -> (u64, bool) {
        if let Some(k) = self.known.get(&source)
            && *k.settings == *settings
        {
            return (k.generation, false);
        }
        self.next_generation += 1;
        (self.next_generation, true)
    }

    /// Starts feeding a text node: `frames` is its picture channel. A node re-created
    /// with the settings the worker last drew gets the cached picture at once;
    /// otherwise it is drawn. Returns the request number to wait on.
    pub fn attach(
        &mut self,
        source: SourceId,
        frames: ParamSender<Option<TextFrame>>,
        settings: TextSettings,
    ) -> u64 {
        self.ensure_running();
        let (generation, _) = self.generation_for(source, &settings);
        let settings = Arc::new(settings);
        let request = self.shared.request();
        self.known.insert(
            source,
            Known {
                settings: Arc::clone(&settings),
                generation,
                attached: true,
            },
        );
        self.send(RenderCmd::Attach {
            source,
            frames,
            settings,
            generation,
            request,
        });
        self.poll_now(source);
        request
    }

    /// New settings for an attached source (an edit, a preview while typing, undo).
    /// `None` when they equal the last ones sent; otherwise the request to wait on.
    pub fn set_settings(&mut self, source: SourceId, settings: TextSettings) -> Option<u64> {
        let (generation, changed) = self.generation_for(source, &settings);
        if !changed {
            return None;
        }
        self.ensure_running();
        let file_changed = self.known.get(&source).is_none_or(|k| {
            k.settings.file != settings.file || k.settings.file_lines != settings.file_lines
        });
        let settings = Arc::new(settings);
        let attached = self.known.get(&source).is_some_and(|k| k.attached);
        self.known.insert(
            source,
            Known {
                settings: Arc::clone(&settings),
                generation,
                attached,
            },
        );
        let request = self.shared.request();
        self.send(RenderCmd::Settings {
            source,
            settings,
            generation,
            request,
        });
        if file_changed {
            if let Some(p) = &self.probe {
                let _ = p.tx.send(ProbeCmd::Forget { source });
            }
            self.poll_now(source);
        }
        Some(request)
    }

    /// Draws `source` again ahead of other work (a source entering the on-air set)
    /// and reads its file now. The request to wait on, if the source is attached.
    pub fn refresh(&mut self, source: SourceId) -> Option<u64> {
        if !self.known.get(&source).is_some_and(|k| k.attached) {
            return None;
        }
        self.ensure_running();
        let request = self.shared.request();
        self.send(RenderCmd::Rerender { source, request });
        self.poll_now(source);
        Some(request)
    }

    /// Stops feeding `source`'s node (it was retired). Its last picture stays cached,
    /// so undoing a delete shows it again at once.
    pub fn detach(&mut self, source: SourceId) {
        if let Some(k) = self.known.get_mut(&source) {
            k.attached = false;
        }
        self.send(RenderCmd::Detach { source });
        if let Some(p) = &self.probe {
            let _ = p.tx.send(ProbeCmd::Forget { source });
        }
        self.last_use = self.now();
    }

    /// The sources whose files are polled and whose live content is kept up to date
    /// (the on-air set, ADR-0129 decision 7).
    pub fn set_on_air(&mut self, sources: BTreeSet<SourceId>) {
        self.on_air = sources;
    }

    /// The installed font families: the cache, or `NotReady` while the first list is
    /// built. Refreshes the cache when it is older than [`FAMILIES_REFRESH`].
    pub fn families(&mut self) -> Families {
        self.ensure_running();
        let now = self.now();
        let cached = self
            .shared
            .families
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let stale = cached
            .as_ref()
            .is_none_or(|(_, at)| now.saturating_since(*at) >= FAMILIES_REFRESH);
        if stale && !self.shared.families_pending.swap(true, Ordering::AcqRel) {
            self.send(RenderCmd::Families);
        }
        match cached {
            Some((list, _)) => Families::Ready(list),
            None => Families::NotReady,
        }
    }

    /// Problems set or cleared since the last call, oldest first.
    pub fn take_problems(&mut self) -> Vec<ProblemUpdate> {
        std::mem::take(
            &mut *self
                .shared
                .problems
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    fn poll_now(&self, source: SourceId) {
        if !self.on_air.contains(&source) || self.hung.contains_key(&source) {
            return;
        }
        let (Some(k), Some(p)) = (self.known.get(&source), &self.probe) else {
            return;
        };
        if !k.attached {
            return;
        }
        if let Some(path) = &k.settings.file {
            let _ = p.tx.send(ProbeCmd::Poll {
                source,
                path: path.clone(),
                file_lines: k.settings.file_lines,
                generation: k.generation,
            });
        }
    }

    /// The control tick: polls on-air files every [`POLL_INTERVAL`], reports calls
    /// that hang, and stops the threads when idle. Does nothing while stopped.
    pub fn tick(&mut self) {
        if self.render.is_none() {
            return;
        }
        let now = self.now();
        self.check_hangs(now);
        let due = self
            .last_poll
            .is_none_or(|at| now.saturating_since(at) >= POLL_INTERVAL);
        if due {
            self.last_poll = Some(now);
            let sources: Vec<SourceId> = self.on_air.iter().copied().collect();
            for s in sources {
                self.poll_now(s);
            }
        }
        let any_attached = self.known.values().any(|k| k.attached);
        if any_attached {
            self.last_use = now;
        } else if now.saturating_since(self.last_use) >= IDLE_STOP_AFTER {
            self.stop();
        }
    }

    fn check_hangs(&mut self, now: Timestamp) {
        let hung_for = |since: u64| {
            since != 0 && now.saturating_since(Timestamp::from_nanos(since - 1)) >= HANG_AFTER
        };
        // Render.
        let since = self.shared.render_busy.load(Ordering::Acquire);
        if hung_for(since) {
            if !self.render_hang_reported {
                self.render_hang_reported = true;
                let raw = self.shared.render_source.load(Ordering::Acquire);
                if raw != 0 {
                    let source = SourceId::from_raw((raw - 1) as u32);
                    self.shared.post((
                        source,
                        ProblemSlot::Render,
                        Some(TextProblem::CouldNotDraw),
                    ));
                }
            }
        } else if since == 0 {
            self.render_hang_reported = false;
        }
        // Calls that came back: poll those paths again.
        let back: Vec<SourceId> = self
            .hung
            .iter()
            .filter(|(_, b)| {
                b.since.load(Ordering::Acquire) == 0 || b.exited.load(Ordering::Acquire)
            })
            .map(|(s, _)| *s)
            .collect();
        for s in back {
            self.hung.remove(&s);
            self.shared.post((s, ProblemSlot::File, None));
        }
        self.detached
            .retain(|p| !p.busy.exited.load(Ordering::Acquire));
        // The active probe.
        let Some(p) = &self.probe else {
            return;
        };
        let since = p.busy.since.load(Ordering::Acquire);
        if !hung_for(since) {
            return;
        }
        let raw = p.busy.source.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        let source = SourceId::from_raw((raw - 1) as u32);
        if self.hung.contains_key(&source) {
            return;
        }
        self.hung.insert(source, Arc::clone(&p.busy));
        self.shared.post((
            source,
            ProblemSlot::File,
            Some(TextProblem::FileNotAnswering),
        ));
        if self.detached_probes() < MAX_DETACHED_PROBES
            && let Some(old) = self.probe.take()
        {
            let _ = old.tx.send(ProbeCmd::Stop);
            self.detached.push(old);
            self.probe = self.spawn_probe();
        }
    }

    /// Stops both threads without waiting on a call that hangs: the render thread is
    /// joined only when idle, a busy probe is left behind (at most
    /// [`MAX_DETACHED_PROBES`]; at the cap it is kept for the next start). Calls
    /// `trim` on the renderers when the render thread was joined.
    pub fn stop(&mut self) {
        if let Some(mut r) = self.render.take() {
            let _ = r.tx.send(RenderCmd::Stop);
            let idle = self.shared.render_busy.load(Ordering::Acquire) == 0;
            if idle {
                if let Some(h) = r.handle.take() {
                    let _ = h.join();
                }
                for renderer in &self.renderers {
                    renderer.trim();
                }
            }
        }
        if let Some(mut p) = self.probe.take() {
            if !p.is_busy() {
                let _ = p.tx.send(ProbeCmd::Stop);
                if let Some(h) = p.handle.take() {
                    let _ = h.join();
                }
            } else if self.detached_probes() < MAX_DETACHED_PROBES {
                let _ = p.tx.send(ProbeCmd::Stop);
                self.detached.push(p);
            } else {
                // At the cap: keep it; the next start uses it again.
                self.probe = Some(p);
            }
        }
        self.known.retain(|_, k| k.attached);
        self.last_poll = None;
        *self
            .shared
            .families
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.shared.families_pending.store(false, Ordering::Release);
    }
}

impl Drop for TextWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---- Render thread -------------------------------------------------------------------

struct Drawn {
    frames: Option<ParamSender<Option<TextFrame>>>,
    settings: Arc<TextSettings>,
    generation: u64,
    file_text: Option<String>,
    last: Option<TextFrame>,
}

fn render_loop(rx: &Receiver<RenderCmd>, shared: &Shared, renderers: &[Arc<dyn TextRenderer>]) {
    let mut sources: BTreeMap<SourceId, Drawn> = BTreeMap::new();
    let mut panicked = false;
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        // Newest request per source; entering sources (re-renders) first.
        let mut front: VecDeque<(SourceId, u64)> = VecDeque::new();
        let mut back: VecDeque<(SourceId, u64)> = VecDeque::new();
        let mut stop = false;
        let mut families = false;
        for cmd in batch {
            match cmd {
                RenderCmd::Attach {
                    source,
                    mut frames,
                    settings,
                    generation,
                    request,
                } => {
                    let cached = sources
                        .get(&source)
                        .and_then(|d| d.last.clone())
                        .filter(|f| f.settings_generation == generation);
                    let file_text = sources.get(&source).and_then(|d| d.file_text.clone());
                    let has_cache = cached.is_some();
                    if let Some(frame) = &cached {
                        drop(frames.recycle());
                        drop(frames.send(Some(frame.clone())));
                    }
                    sources.insert(
                        source,
                        Drawn {
                            frames: Some(frames),
                            settings,
                            generation,
                            file_text,
                            last: cached,
                        },
                    );
                    if !has_cache {
                        queue(&mut back, source, request);
                    }
                }
                RenderCmd::Settings {
                    source,
                    settings,
                    generation,
                    request,
                } => {
                    let d = sources.entry(source).or_insert_with(|| Drawn {
                        frames: None,
                        settings: Arc::clone(&settings),
                        generation,
                        file_text: None,
                        last: None,
                    });
                    if d.settings.file != settings.file {
                        d.file_text = None;
                    }
                    d.settings = settings;
                    d.generation = generation;
                    queue(&mut back, source, request);
                }
                RenderCmd::File {
                    source,
                    generation,
                    content,
                    request,
                } => {
                    if let Some(d) = sources.get_mut(&source)
                        && d.generation == generation
                    {
                        d.file_text = Some(content);
                        queue(&mut back, source, request);
                    }
                }
                RenderCmd::Rerender { source, request } => {
                    queue(&mut front, source, request);
                }
                RenderCmd::Detach { source } => {
                    if let Some(d) = sources.get_mut(&source) {
                        d.frames = None;
                    }
                }
                RenderCmd::Families => families = true,
                RenderCmd::Stop => stop = true,
            }
        }
        if stop {
            break;
        }
        if families {
            list_families(shared, renderers);
        }
        for (source, request) in front.into_iter().chain(back) {
            if let Some(d) = sources.get_mut(&source)
                && d.frames.is_some()
            {
                if panicked {
                    for r in renderers {
                        r.trim();
                    }
                }
                panicked = draw(shared, renderers, source, d, request);
            }
        }
    }
}

fn queue(q: &mut VecDeque<(SourceId, u64)>, source: SourceId, request: u64) {
    match q.iter_mut().find(|(s, _)| *s == source) {
        Some(entry) => entry.1 = entry.1.max(request),
        None => q.push_back((source, request)),
    }
}

fn list_families(shared: &Shared, renderers: &[Arc<dyn TextRenderer>]) {
    let mut all = BTreeSet::new();
    for r in renderers {
        if let Ok(Ok(list)) = catch_unwind(AssertUnwindSafe(|| r.families())) {
            all.extend(list);
        }
    }
    let list = Arc::new(all.into_iter().collect::<Vec<_>>());
    *shared
        .families
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some((list, shared.clock.now()));
    shared.families_pending.store(false, Ordering::Release);
}

/// Draws one source and sends the picture. Returns whether the renderer panicked.
fn draw(
    shared: &Shared,
    renderers: &[Arc<dyn TextRenderer>],
    source: SourceId,
    d: &mut Drawn,
    request: u64,
) -> bool {
    let mut block: TextBlock = d.settings.block.clone();
    if d.settings.file.is_some() {
        block.text = d.file_text.clone().unwrap_or_default();
    }
    let Some(renderer) = pick_renderer(renderers, &block) else {
        shared.post((
            source,
            ProblemSlot::Render,
            Some(TextProblem::RenderFailed(
                "No text renderer is installed".into(),
            )),
        ));
        return false;
    };
    if let Some(frames) = &mut d.frames {
        // Drop the picture the node handed back here, not on the render thread.
        drop(frames.recycle());
    }
    shared
        .render_source
        .store(u64::from(source.get()) + 1, Ordering::Release);
    shared.render_busy.store(shared.stamp(), Ordering::Release);
    let result = catch_unwind(AssertUnwindSafe(|| renderer.render(&block)));
    shared.render_busy.store(0, Ordering::Release);
    shared.render_source.store(0, Ordering::Release);
    match result {
        Ok(Ok((picture, notes))) => {
            let Some(frame) = TextFrame::new(request, d.generation, Arc::new(picture)) else {
                return false;
            };
            if let Some(frames) = &mut d.frames {
                drop(frames.send(Some(frame.clone())));
            }
            d.last = Some(frame);
            let note = notes.font_missing.then_some(TextProblem::FontMissing);
            shared.post((source, ProblemSlot::Render, note));
            false
        }
        Ok(Err(e)) => {
            shared.post((
                source,
                ProblemSlot::Render,
                Some(TextProblem::RenderFailed(e.to_string())),
            ));
            false
        }
        Err(_) => {
            shared.post((
                source,
                ProblemSlot::Render,
                Some(TextProblem::RenderFailed(
                    "The text renderer stopped unexpectedly".into(),
                )),
            ));
            true
        }
    }
}

// ---- Probe thread --------------------------------------------------------------------

struct ProbeCtx {
    files: Arc<dyn FileProbe>,
    wall: Arc<dyn TextClock>,
    shared: Arc<Shared>,
    busy: Arc<ProbeBusy>,
    render: Sender<RenderCmd>,
}

#[derive(Default)]
struct Watched {
    generation: u64,
    meta: Option<FileMeta>,
    accepted: Option<u64>,
    candidate: Option<u64>,
    missing: bool,
    not_text: bool,
}

fn probe_loop(rx: &Receiver<ProbeCmd>, ctx: &ProbeCtx) {
    let mut watched: BTreeMap<SourceId, Watched> = BTreeMap::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            ProbeCmd::Stop => break,
            ProbeCmd::Forget { source } => {
                watched.remove(&source);
            }
            ProbeCmd::Poll {
                source,
                path,
                file_lines,
                generation,
            } => {
                let w = watched.entry(source).or_default();
                if w.generation != generation {
                    *w = Watched {
                        generation,
                        ..Watched::default()
                    };
                }
                ctx.busy
                    .source
                    .store(u64::from(source.get()) + 1, Ordering::Release);
                ctx.busy.since.store(ctx.shared.stamp(), Ordering::Release);
                poll(ctx, source, &path, file_lines, w);
                ctx.busy.since.store(0, Ordering::Release);
            }
        }
    }
    ctx.busy.exited.store(true, Ordering::Release);
}

fn poll(ctx: &ProbeCtx, source: SourceId, path: &str, file_lines: u32, w: &mut Watched) {
    let generation = w.generation;
    let send = |content: String| {
        let _ = ctx.render.send(RenderCmd::File {
            source,
            generation,
            content,
            request: ctx.shared.request(),
        });
    };
    let found = ctx
        .files
        .canonicalize(Path::new(path))
        .and_then(|canon| Ok((ctx.files.metadata(&canon)?, canon)));
    let (meta, canon) = match found {
        Ok((Some(meta), canon)) => (meta, canon),
        _ => {
            if !w.missing {
                *w = Watched {
                    generation,
                    missing: true,
                    ..Watched::default()
                };
                ctx.shared
                    .post((source, ProblemSlot::File, Some(TextProblem::FileNotFound)));
                send(String::new());
            }
            return;
        }
    };
    let now = ctx.wall.now();
    let recent = meta.modified.is_some_and(|m| {
        now.duration_since(m)
            .map_or(true, |age| age < RECENT_WINDOW)
    });
    // Unchanged since the last look and not recent: nothing to read, unless a
    // change is waiting for its agreeing poll.
    if w.meta == Some(meta)
        && !recent
        && w.candidate.is_none()
        && (w.accepted.is_some() || w.not_text)
    {
        return;
    }
    w.meta = Some(meta);
    let Ok((head, tail)) = ctx.files.read(&canon, 3, MAX_FILE_BYTES) else {
        return;
    };
    let hash = content_hash(&tail) ^ meta.len;
    if w.accepted == Some(hash) {
        w.candidate = None;
        return;
    }
    let first = w.accepted.is_none() && !w.not_text;
    let agreed = w.candidate == Some(hash) || (first && !recent && !tail.is_empty());
    if !agreed {
        w.candidate = Some(hash);
        return;
    }
    w.candidate = None;
    w.accepted = Some(hash);
    match decode(&head, &tail, meta.len <= MAX_FILE_BYTES) {
        Ok(text) => {
            if w.missing || w.not_text || first {
                ctx.shared.post((source, ProblemSlot::File, None));
            }
            w.not_text = false;
            w.missing = false;
            send(last_lines(&text, file_lines));
        }
        Err(FileTextError::NotText) => {
            w.not_text = true;
            ctx.shared
                .post((source, ProblemSlot::File, Some(TextProblem::NotText)));
        }
    }
}
