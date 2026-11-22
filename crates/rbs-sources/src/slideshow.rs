//! The image slide show source (ADR-0171): its settings schema, the control handle and
//! the shared decode thread.
//!
//! Zero cost when unused (P13): no thread exists until a slide show node is warm, and
//! the thread exits as soon as none is. A source that is not warm holds no pictures.
//! The thread decodes and fits at most three pictures per source (previous, current,
//! next) and hands them to the node through its parameter channel, so displaced
//! pictures drop here, never on the render thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rbs_core::time::Timestamp;
use rbs_gpu::SlideKind;
use rbs_graph::runtime::param::ParamSender;
use rbs_nodes::{SlideFrame, SlideshowShared};
use rbs_params::{Field, Schema, Values};
use rbs_scene::model::{SettingValue, Settings, SourceId};

use crate::slideshow_files::{Fit, Listing, Shuffler, fit_picture, list_folder};
use crate::still::decode_picture;

/// Settings key: the folder of pictures (text, a local path). Not in the schema: the
/// host owns the path row and its local-path check (ADR-0171 decision 4).
pub const FOLDER: &str = "folder";

/// Choice options.
const ADVANCE: &[(&str, &str)] = &[("auto", "Automatic"), ("manual", "Manual")];
const ORDER: &[(&str, &str)] = &[("sequential", "In order"), ("shuffle", "Shuffle")];
const TRANSITION: &[(&str, &str)] = &[
    ("cut", "Cut"),
    ("fade", "Fade"),
    ("slide", "Slide"),
    ("swipe", "Swipe"),
];
const FIT: &[(&str, &str)] = &[
    ("contain", "Fit inside"),
    ("cover", "Fill"),
    ("stretch", "Stretch"),
];

/// The slide show's settings schema (ADR-0171 decision 4), in display order.
pub const FIELDS: &[Field] = &[
    Field::choice("advance", "Advance", ADVANCE, "auto")
        .with_help("Automatic changes the picture on a timer; Manual waits for Next or Previous."),
    Field::number("interval_s", "Show each", 1.0, 3600.0, 5.0, 0.5, "s").log(),
    Field::choice("order", "Order", ORDER, "sequential"),
    Field::toggle("loop", "Loop", true).with_help("Start again after the last picture."),
    Field::choice("transition", "Transition", TRANSITION, "fade"),
    Field::number(
        "transition_ms",
        "Transition duration",
        0.0,
        5000.0,
        700.0,
        50.0,
        "ms",
    ),
    Field::choice("fit", "Fit", FIT, "contain"),
    Field::toggle("watch", "Watch folder", false)
        .with_help("Pick up pictures added to or removed from the folder."),
];

/// Field indices into [`FIELDS`].
mod at {
    pub const ADVANCE: usize = 0;
    pub const INTERVAL: usize = 1;
    pub const ORDER: usize = 2;
    pub const LOOP: usize = 3;
    pub const TRANSITION: usize = 4;
    pub const TRANSITION_MS: usize = 5;
    pub const FIT: usize = 6;
    pub const WATCH: usize = 7;
}

/// The keys whose change keeps the node (ADR-0108 decision 12, ADR-0171 decision 4):
/// every schema key except `transition`.
pub const LIVE_KEYS: &[&str] = &[
    "advance",
    "interval_s",
    "order",
    "loop",
    "transition_ms",
    "fit",
    "watch",
];

/// The option keys (ADR-0133 decision 2): every schema key. The folder and the size
/// are identity keys.
pub const OPTION_KEYS: &[&str] = &[
    "advance",
    "interval_s",
    "order",
    "loop",
    "transition",
    "transition_ms",
    "fit",
    "watch",
];

/// Every key the type uses: the schema, the folder and the output size (decision 5).
pub const KEYS: &[&str] = &[
    "advance",
    "interval_s",
    "order",
    "loop",
    "transition",
    "transition_ms",
    "fit",
    "watch",
    FOLDER,
    crate::types::keys::WIDTH,
    crate::types::keys::HEIGHT,
];

/// The largest output side (as for other sources).
const MAX_SIDE: i64 = 16_384;

/// Checks the settings of an edit: only [`KEYS`], schema values in range, the folder as
/// text, and the size as two positive sides. `(key, expected)` on a refusal.
pub fn validate(settings: &Settings) -> Result<(), (&'static str, &'static str)> {
    if settings.keys().any(|k| !KEYS.contains(&k.as_str())) {
        return Err(("settings", "only keys this source type uses"));
    }
    if check_settings(settings).is_err() {
        return Err(("settings", "values within the slide show's ranges"));
    }
    if matches!(settings.get(FOLDER), Some(v) if !matches!(v, SettingValue::Text(_))) {
        return Err((FOLDER, "text"));
    }
    let side = |key: &'static str| match settings.get(key) {
        None => Ok(None),
        Some(SettingValue::Int(n)) if (1..=MAX_SIDE).contains(n) => Ok(Some(*n)),
        Some(_) => Err((key, "a whole number of pixels from 1 to 16384")),
    };
    let (w, h) = (
        side(crate::types::keys::WIDTH)?,
        side(crate::types::keys::HEIGHT)?,
    );
    if w.is_some() != h.is_some() {
        return Err((crate::types::keys::WIDTH, "set together with height"));
    }
    Ok(())
}

/// The output size of a slide show: its `width` and `height`, else the main canvas's,
/// else [`crate::still::DEFAULT_SIZE`] (ADR-0171 decision 5).
pub fn output_size(settings: &Settings, canvas: Option<(u32, u32)>) -> (u32, u32) {
    let side = |key: &str| match settings.get(key) {
        Some(SettingValue::Int(n)) if (1..=MAX_SIDE).contains(n) => u32::try_from(*n).ok(),
        _ => None,
    };
    match (
        side(crate::types::keys::WIDTH),
        side(crate::types::keys::HEIGHT),
    ) {
        (Some(w), Some(h)) => (w, h),
        _ => canvas.unwrap_or(crate::still::DEFAULT_SIZE),
    }
}

/// A slide show's node instance and what the host attaches to the hub.
pub struct SlideshowSource {
    /// The node.
    pub node: rbs_nodes::SlideshowNode,
    /// The node's picture channel; the hub keeps it.
    pub frames: ParamSender<Option<SlideFrame>>,
    /// What the node, the hub and the host share.
    pub shared: Arc<SlideshowShared>,
    /// Its settings, read leniently.
    pub settings: SlideshowSettings,
    /// Its output size.
    pub size: (u32, u32),
}

/// Builds the instance (no file access: the folder is read on the decode thread).
pub fn instantiate(settings: &Settings, canvas: Option<(u32, u32)>) -> SlideshowSource {
    let read = read_settings(settings);
    let size = output_size(settings, canvas);
    let shared = Arc::new(SlideshowShared::default());
    write_live(&shared, &read);
    let (node, frames) =
        rbs_nodes::SlideshowNode::new(size, read.transition, Arc::clone(&shared), None);
    SlideshowSource {
        node,
        frames,
        shared,
        settings: read,
        size,
    }
}

/// The slide show's settings, read leniently.
#[derive(Debug, Clone, PartialEq)]
pub struct SlideshowSettings {
    /// The folder, when set.
    pub folder: Option<PathBuf>,
    /// Milliseconds per picture; `None` for manual.
    pub interval_ms: Option<u64>,
    /// Shuffle passes instead of name order.
    pub shuffle: bool,
    /// Start again after the last picture.
    pub looped: bool,
    /// The change, `None` for a cut.
    pub transition: Option<SlideKind>,
    /// Change length before the half-interval cap.
    pub transition_ms: u32,
    /// How pictures fill the output.
    pub fit: Fit,
    /// Re-list the folder while warm.
    pub watch: bool,
}

/// The schema keys of `settings` only (the folder and size keys are not fields).
fn schema_part(settings: &Settings) -> Settings {
    settings
        .iter()
        .filter(|(k, _)| FIELDS.iter().any(|f| f.key == k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Strict check for an edit: wrong kinds, unknown choices and out-of-range numbers are
/// refused (the folder is checked by the host).
pub fn check_settings(settings: &Settings) -> Result<(), rbs_params::ParamError> {
    crate::filters::filter_values(&FIELDS, &schema_part(settings)).map(|_| ())
}

/// The schema values of the settings, read leniently (the Source tab's rows).
pub fn values(settings: &Settings) -> Values {
    crate::filters::filter_values_lenient(&FIELDS, &schema_part(settings)).0
}

/// Reads the settings leniently (a document value out of range is clamped).
pub fn read_settings(settings: &Settings) -> SlideshowSettings {
    let (v, _) = crate::filters::filter_values_lenient(&FIELDS, &schema_part(settings));
    from_values(&v, settings)
}

fn from_values(v: &Values, settings: &Settings) -> SlideshowSettings {
    let folder = match settings.get(FOLDER) {
        Some(SettingValue::Text(t)) if !t.trim().is_empty() => Some(PathBuf::from(t)),
        _ => None,
    };
    let manual = v.choice(at::ADVANCE) == 1;
    SlideshowSettings {
        folder,
        interval_ms: (!manual).then(|| (f64::from(v.number(at::INTERVAL)) * 1000.0) as u64),
        shuffle: v.choice(at::ORDER) == 1,
        looped: v.toggle(at::LOOP),
        transition: match v.choice(at::TRANSITION) {
            1 => Some(SlideKind::Fade),
            2 => Some(SlideKind::Slide),
            3 => Some(SlideKind::Swipe),
            _ => None,
        },
        transition_ms: v.number(at::TRANSITION_MS) as u32,
        fit: match v.choice(at::FIT) {
            1 => Fit::Cover,
            2 => Fit::Stretch,
            _ => Fit::Contain,
        },
        watch: v.toggle(at::WATCH),
    }
}

/// The schema as a trait object, for hosts that draw it.
pub fn schema() -> &'static dyn Schema {
    &FIELDS
}

/// A command from the control API or a shortcut (ADR-0171 decision 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideCommand {
    /// The next picture.
    Next,
    /// The previous picture.
    Previous,
    /// The first picture of a new pass.
    Restart,
}

/// What the host reads about a slide show for `media.state` (ADR-0171 decision 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlideState {
    /// Index of the picture on the output in the folder's order (0-based).
    pub index: Option<usize>,
    /// Pictures listed.
    pub count: usize,
    /// Matching files past the cap.
    pub left_out: usize,
    /// The last picture with loop off.
    pub ended: bool,
    /// The folder could not be read.
    pub folder_error: bool,
    /// The timer is stopped (`media.pause`).
    pub paused: bool,
    /// The node is Active: shown on Program, or as a preview it plays.
    pub warm: bool,
    /// How long each picture shows; `None` for `advance = manual`.
    pub interval: Option<Duration>,
    /// How long the picture on the output has been shown, by the `now` given.
    pub shown_for: Duration,
}

/// How often a watched folder is listed again while warm.
pub const WATCH_EVERY: Duration = Duration::from_secs(2);
/// The longest the thread sleeps while a source is warm.
const MAX_SLEEP: Duration = Duration::from_secs(2);

/// One slide show the hub serves.
struct Slot {
    settings: SlideshowSettings,
    size: (u32, u32),
    shared: Arc<SlideshowShared>,
    sender: ParamSender<Option<SlideFrame>>,
    listing: Option<Listing>,
    listed_at: Option<Instant>,
    folder_error: bool,
    /// Show order: indices into the listing.
    order: Vec<usize>,
    /// Position in `order` of the picture on the output (or to show first).
    pos: usize,
    /// Fitted pictures by listing index (at most three).
    cache: Vec<(usize, Arc<Vec<u8>>)>,
    /// Last serial sent, and the order position it holds.
    sent: Option<(u64, usize)>,
    /// A command to run on the next wake.
    command: Option<SlideCommand>,
    /// The fit changed: send the current picture again, as a cut.
    refit: bool,
    shuffler: Shuffler,
    ended: bool,
    /// Files in a row that did not decode; at the listing's length the slot stops
    /// until the next listing or command, so a folder of broken files never spins.
    failures: usize,
}

impl Slot {
    fn index_at(&self, pos: usize) -> Option<usize> {
        self.order.get(pos).copied()
    }

    fn reorder(&mut self) {
        let count = self.listing.as_ref().map_or(0, |l| l.entries.len());
        let keep = self.index_at(self.pos);
        self.order = if self.settings.shuffle {
            self.shuffler.pass(count, None)
        } else {
            (0..count).collect()
        };
        // Keep showing the same picture after a re-list or an order change.
        self.pos = keep
            .and_then(|i| self.order.iter().position(|&o| o == i))
            .unwrap_or(0);
    }

    /// The order position after `pos`, starting a new pass at the end.
    fn after(&mut self, pos: usize) -> Option<usize> {
        if pos + 1 < self.order.len() {
            return Some(pos + 1);
        }
        if !self.settings.looped || self.order.is_empty() {
            return None;
        }
        if self.settings.shuffle {
            let last = self.order.last().copied();
            let count = self.order.len();
            self.order = self.shuffler.pass(count, last);
        }
        Some(0)
    }

    fn before(&self, pos: usize) -> Option<usize> {
        if pos > 0 {
            Some(pos - 1)
        } else if self.settings.looped && !self.order.is_empty() {
            Some(self.order.len() - 1)
        } else {
            None
        }
    }
}

/// The work of one wake for one slot, done outside the lock.
enum Job {
    /// List the folder.
    List(PathBuf),
    /// Decode and fit the picture at a listing index, to send at an order position.
    Decode {
        path: PathBuf,
        index: usize,
        pos: usize,
        now: bool,
        cut: bool,
        size: (u32, u32),
        fit: Fit,
    },
}

#[derive(Default)]
struct HubState {
    slots: BTreeMap<SourceId, Slot>,
    serial: u64,
    stop: bool,
}

/// The control-side owner of the slide shows and their shared decode thread.
pub struct SlideshowHub {
    state: Arc<Mutex<HubState>>,
    thread: Option<JoinHandle<()>>,
}

impl Default for SlideshowHub {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SlideshowHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlideshowHub")
            .field("running", &self.thread.is_some())
            .finish()
    }
}

fn lock(state: &Mutex<HubState>) -> std::sync::MutexGuard<'_, HubState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SlideshowHub {
    /// An empty hub: no thread.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HubState::default())),
            thread: None,
        }
    }

    /// Serves `source`'s node from now on. Replaces an earlier attach of the source.
    pub fn attach(
        &mut self,
        source: SourceId,
        settings: SlideshowSettings,
        size: (u32, u32),
        shared: Arc<SlideshowShared>,
        sender: ParamSender<Option<SlideFrame>>,
    ) {
        write_live(&shared, &settings);
        let seed = u64::from(source.get()).wrapping_mul(0x9E37_79B9) ^ 0x5DEE_CE66;
        lock(&self.state).slots.insert(
            source,
            Slot {
                settings,
                size,
                shared,
                sender,
                listing: None,
                listed_at: None,
                folder_error: false,
                order: Vec::new(),
                pos: 0,
                cache: Vec::new(),
                sent: None,
                command: None,
                refit: false,
                shuffler: Shuffler::new(seed),
                ended: false,
                failures: 0,
            },
        );
        self.wake();
    }

    /// Stops serving `source` (its node left the graph).
    pub fn detach(&mut self, source: SourceId) {
        lock(&self.state).slots.remove(&source);
    }

    /// Keeps only `live` (sources still bound).
    pub fn retain(&mut self, live: impl Fn(SourceId) -> bool) {
        lock(&self.state).slots.retain(|id, _| live(*id));
    }

    /// The slide shows served.
    pub fn sources(&self) -> Vec<SourceId> {
        lock(&self.state).slots.keys().copied().collect()
    }

    /// True when `source` is served.
    pub fn has(&self, source: SourceId) -> bool {
        lock(&self.state).slots.contains_key(&source)
    }

    /// Applies live keys (ADR-0171 decision 4).
    pub fn set_settings(&mut self, source: SourceId, settings: SlideshowSettings) {
        {
            let mut state = lock(&self.state);
            let Some(slot) = state.slots.get_mut(&source) else {
                return;
            };
            write_live(&slot.shared, &settings);
            let order_changed = slot.settings.shuffle != settings.shuffle;
            slot.refit |= slot.settings.fit != settings.fit;
            if slot.refit {
                slot.cache.clear();
            }
            slot.settings = settings;
            if order_changed {
                slot.reorder();
                // The prefetched next was drawn from the old order.
                slot.sent = slot.sent.map(|(serial, _)| (serial, usize::MAX));
            }
            if !slot.settings.looped {
                slot.ended = false;
            }
        }
        self.wake();
    }

    /// Pauses or resumes the timer (`media.pause`, `media.play`).
    pub fn set_paused(&self, source: SourceId, paused: bool) -> bool {
        let state = lock(&self.state);
        let Some(slot) = state.slots.get(&source) else {
            return false;
        };
        slot.shared.paused.store(paused, Ordering::Relaxed);
        true
    }

    /// Runs a command on the next wake; false when the source is not served.
    pub fn command(&self, source: SourceId, command: SlideCommand) -> bool {
        {
            let mut state = lock(&self.state);
            let Some(slot) = state.slots.get_mut(&source) else {
                return false;
            };
            slot.command = Some(command);
        }
        self.wake();
        true
    }

    /// What `media.state` reports; `now` is the master clock's time.
    pub fn state(&self, source: SourceId, now: Timestamp) -> Option<SlideState> {
        let state = lock(&self.state);
        let slot = state.slots.get(&source)?;
        let shown = slot.shared.shown.load(Ordering::Acquire);
        let index = match slot.sent {
            Some((serial, pos)) if serial == shown => slot.index_at(pos),
            _ if shown != 0 => slot.index_at(slot.pos),
            _ => None,
        };
        Some(SlideState {
            index,
            count: slot.listing.as_ref().map_or(0, |l| l.entries.len()),
            left_out: slot.listing.as_ref().map_or(0, |l| l.left_out),
            ended: slot.ended,
            folder_error: slot.folder_error,
            paused: slot.shared.paused.load(Ordering::Relaxed),
            warm: slot.shared.warm.load(Ordering::Acquire),
            interval: slot.shared.interval(),
            shown_for: match shown {
                0 => Duration::ZERO,
                _ => Duration::from_nanos(
                    now.as_nanos()
                        .saturating_sub(slot.shared.shown_at_ns.load(Ordering::Acquire)),
                ),
            },
        })
    }

    /// Starts the thread when a served node is warm, and joins it after it left
    /// because none is. Call it from the control loop (`Studio::service`).
    pub fn service(&mut self) {
        let any_warm = lock(&self.state)
            .slots
            .values()
            .any(|s| s.shared.warm.load(Ordering::Acquire));
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
        if any_warm && self.thread.is_none() {
            let state = Arc::clone(&self.state);
            self.thread = std::thread::Builder::new()
                .name("rbs-slideshow".to_owned())
                .spawn(move || run(&state))
                .ok();
        }
    }

    /// True while the decode thread runs.
    pub fn running(&self) -> bool {
        self.thread.is_some()
    }

    /// Wakes the decode thread now instead of at its next timeout (a test that plays
    /// the node, or a host that knows a picture was shown).
    pub fn nudge(&self) {
        self.wake();
    }

    fn wake(&self) {
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }
}

impl Drop for SlideshowHub {
    fn drop(&mut self) {
        lock(&self.state).stop = true;
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn write_live(shared: &SlideshowShared, s: &SlideshowSettings) {
    shared
        .interval_ms
        .store(s.interval_ms.unwrap_or(0), Ordering::Relaxed);
    shared
        .transition_ms
        .store(s.transition_ms, Ordering::Relaxed);
}

/// The decode thread: exits when no served node is warm.
fn run(state: &Mutex<HubState>) {
    loop {
        let mut jobs = Vec::new();
        let mut sleep = MAX_SLEEP;
        {
            let mut st = lock(state);
            if st.stop {
                return;
            }
            let mut any_warm = false;
            for (id, slot) in &mut st.slots {
                if !slot.shared.warm.load(Ordering::Acquire) {
                    // Standby holds no pictures; the index stays (decision 6).
                    slot.cache.clear();
                    slot.sent = None;
                    continue;
                }
                any_warm = true;
                if let Some(ms) = slot.settings.interval_ms {
                    sleep = sleep.min(Duration::from_millis(ms / 2).max(Duration::from_millis(50)));
                }
                if let Some(job) = next_job(slot) {
                    jobs.push((*id, job));
                }
            }
            if !any_warm {
                return;
            }
        }
        // Work begets work (a listing, then the first picture): sleep only when idle.
        let idle = jobs.is_empty();
        for (id, job) in jobs {
            let done = do_job(&job);
            let mut st = lock(state);
            let serial = st.serial + 1;
            let Some(slot) = st.slots.get_mut(&id) else {
                continue;
            };
            if finish(slot, job, done, serial) {
                st.serial = serial;
            }
        }
        if idle {
            std::thread::park_timeout(sleep);
        }
    }
}

/// What a job produced.
enum Done {
    Listed(Result<Listing, String>),
    Picture(Result<Arc<Vec<u8>>, String>),
}

fn do_job(job: &Job) -> Done {
    match job {
        Job::List(folder) => Done::Listed(list_folder(folder)),
        Job::Decode {
            path, size, fit, ..
        } => Done::Picture(decode_picture(path).map(|p| Arc::new(fit_picture(&p, *size, *fit).2))),
    }
}

/// The one job a warm slot needs now, if any.
fn next_job(slot: &mut Slot) -> Option<Job> {
    let folder = slot.settings.folder.clone()?;
    let due_list = slot.listing.is_none()
        || (slot.settings.watch && slot.listed_at.is_none_or(|t| t.elapsed() >= WATCH_EVERY));
    if due_list {
        return Some(Job::List(folder));
    }
    if slot.order.is_empty() || (slot.failures >= slot.order.len() && slot.command.is_none()) {
        return None;
    }
    let shown = slot.shared.shown.load(Ordering::Acquire);
    // Where the output is, in the order.
    if let Some((serial, pos)) = slot.sent
        && serial == shown
        && pos != usize::MAX
    {
        slot.pos = pos;
    }
    let (pos, now, cut) = if let Some(cmd) = slot.command.take() {
        let pos = match cmd {
            SlideCommand::Next => slot.after(slot.pos),
            SlideCommand::Previous => slot.before(slot.pos),
            SlideCommand::Restart => {
                if slot.settings.shuffle {
                    slot.order = slot
                        .shuffler
                        .pass(slot.order.len(), slot.index_at(slot.pos));
                }
                Some(0)
            }
        }?;
        slot.ended = false;
        (pos, true, false)
    } else if std::mem::take(&mut slot.refit) || slot.sent.is_none() {
        // The first picture, or the current one again after a fit change.
        (slot.pos.min(slot.order.len() - 1), true, true)
    } else if slot.sent.is_some_and(|(serial, _)| serial == shown) {
        // The node took the last one: prefetch the next.
        match slot.after(slot.pos) {
            Some(pos) => (pos, false, false),
            None => {
                slot.ended = true;
                return None;
            }
        }
    } else {
        return None;
    };
    let index = slot.index_at(pos)?;
    let path = slot.listing.as_ref()?.path(&folder, index)?;
    Some(Job::Decode {
        path,
        index,
        pos,
        now,
        cut,
        size: slot.size,
        fit: slot.settings.fit,
    })
}

/// Applies a job's result; true when a frame was sent with `serial`.
fn finish(slot: &mut Slot, job: Job, done: Done, serial: u64) -> bool {
    match (job, done) {
        (Job::List(_), Done::Listed(Ok(listing))) => {
            let changed = slot.listing.as_ref() != Some(&listing);
            slot.listing = Some(listing);
            slot.listed_at = Some(Instant::now());
            slot.folder_error = false;
            if changed {
                slot.cache.clear();
                slot.reorder();
                slot.failures = 0;
            }
            false
        }
        (Job::List(_), Done::Listed(Err(_))) => {
            slot.listed_at = Some(Instant::now());
            slot.folder_error = true;
            slot.listing.get_or_insert_with(Listing::default);
            false
        }
        (
            Job::Decode {
                index,
                pos,
                now,
                cut,
                size,
                ..
            },
            Done::Picture(result),
        ) => {
            let pixels = match slot.cache.iter().find(|(i, _)| *i == index) {
                Some((_, p)) => Arc::clone(p),
                None => match result {
                    Ok(p) => p,
                    Err(_) => {
                        // A file that does not decode is skipped: step past it.
                        slot.failures += 1;
                        slot.sent = Some((0, pos));
                        slot.pos = pos;
                        return false;
                    }
                },
            };
            slot.failures = 0;
            slot.cache.retain(|(i, _)| *i != index);
            slot.cache.push((index, Arc::clone(&pixels)));
            if slot.cache.len() > 3 {
                slot.cache.remove(0);
            }
            let Some(frame) = SlideFrame::new(serial, now, cut, size, pixels) else {
                return false;
            };
            if now {
                slot.pos = pos;
            }
            slot.sent = Some((serial, pos));
            // The displaced frame drops here, on this thread.
            drop(slot.sender.send(Some(frame)));
            true
        }
        _ => false,
    }
}

/// The folder of a slide show's settings, for the host's local-path check.
pub fn folder_of(settings: &Settings) -> Option<&Path> {
    match settings.get(FOLDER) {
        Some(SettingValue::Text(t)) if !t.trim().is_empty() => Some(Path::new(t.as_str())),
        _ => None,
    }
}
